//! Persistent copy ledger — tracks every position this bot has entered, including
//! which target wallet the copy originated from.
//!
//! ## Design
//!
//! The ledger is the authoritative record of **intent**: it answers "did we copy token X
//! from target T?", which the live Polymarket API alone cannot answer after the fact because
//! a position that has been closed no longer appears in the `/positions` endpoint.
//!
//! A token can have several source lots in the ledger. The paper inventory is aggregated
//! by token, while these lots preserve which target's BUYs each share came from.
//!
//! ## Persistence (atomic — Gap 1)
//!
//! Written atomically: content is first written to `copy_ledger.json.tmp`, then
//! `rename()`d over `copy_ledger.json`.  On POSIX systems rename() is atomic, so a
//! crash mid-write can never leave a corrupt or truncated ledger file.
//!
//! ## Partial-fill tracking (Gap 4)
//!
//! `CopyEntry.filled_size` records the actual number of shares filled (updated by the
//! 30s position-sync task).  The `size` field retains the original order size so both
//! values are available for diagnostics.
//!
//! ## Pruning (Gap 11)
//!
//! `prune_closed_older_than(days)` removes closed entries whose `closed_at` timestamp
//! is older than `days` UTC days.  Call this at startup after `reconcile`.

use crate::models::Position;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const LEDGER_PATH: &str = "copy_ledger.json";

fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A single copy-trade record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyEntry {
    #[serde(default)]
    pub entry_order_id: Option<String>,
    /// Polymarket outcome token ID (U256 as decimal string).
    pub token_id: String,
    /// Target wallet we copied this position from (normalized to lowercase).
    pub source_wallet: String,
    /// Number of shares in the SUBMITTED order (may exceed actual fill).
    pub size: Decimal,
    /// Actual shares filled, updated by the position-sync task.
    /// `None` until the first 30s sync that confirms a fill.
    pub filled_size: Option<Decimal>,
    /// Shares exited from this lot, paired with remaining filled_size for source accounting.
    #[serde(default)]
    pub realized_shares: Decimal,
    /// Realized net PnL attributable to this source lot, including allocated sell fees.
    #[serde(default)]
    pub realized_pnl: Decimal,
    /// Limit price used when submitting the entry order.
    pub entry_price: Decimal,
    /// Original leader BUY value in USDC, before our paper execution or partial fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_notional_usd: Option<Decimal>,
    /// When we submitted the entry order.
    pub copied_at: DateTime<Utc>,
    /// Whether this position has been closed.
    pub closed: bool,
    /// When the position was closed (if applicable).
    pub closed_at: Option<DateTime<Utc>>,
}

/// In-memory copy of the on-disk ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyLedger {
    pub entries: Vec<CopyEntry>,
    /// Start of reliable per-source accounting. Older closed lots have no stored exits.
    #[serde(default = "now_utc")]
    pub performance_since: DateTime<Utc>,
    /// Path to the backing JSON file.  Empty string means in-memory only
    /// (no disk persistence) — used in unit tests.
    #[serde(skip)]
    path: String,
}

impl Default for CopyLedger {
    /// Returns an **in-memory** ledger with no disk backing.
    /// Use [`CopyLedger::load`] or [`CopyLedger::load_from`] to get a
    /// persisted ledger.
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            performance_since: Utc::now(),
            path: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Core operations
// ---------------------------------------------------------------------------

impl CopyLedger {
    pub fn is_persistent(&self) -> bool {
        !self.path.is_empty()
    }
    // -- Constructors -------------------------------------------------------

    /// Load from `copy_ledger.json`, or start an empty ledger only if the file
    /// does not exist. Malformed data must stop startup rather than erase provenance.
    pub fn load() -> Self {
        Self::load_from(LEDGER_PATH)
    }

    /// Load from an arbitrary path.  Useful for testing with temp files.
    pub fn load_from(path: &str) -> Self {
        Self::load_from_checked(path).expect("copy ledger must load without data loss")
    }

    pub fn load_for_paper(path: &str, has_positions: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !has_positions || std::path::Path::new(path).exists(),
            "Paper positions exist but copy ledger is missing at {path}"
        );
        Self::load_from_checked(path)
    }

    pub fn load_from_checked(path: &str) -> anyhow::Result<Self> {
        let mut ledger = match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str::<Self>(&content)
                .map_err(|e| anyhow::anyhow!("Invalid copy ledger at {path}: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("copy_ledger not found at {path} — starting a new ledger.");
                let ledger = Self {
                    path: path.to_string(),
                    ..Default::default()
                };
                ledger.try_save()?;
                return Ok(ledger);
            }
            Err(e) => return Err(e.into()),
        };
        ledger.path = path.to_string();
        Ok(ledger)
    }

    pub fn validate_paper_positions(
        &self,
        positions: &HashMap<String, Position>,
    ) -> anyhow::Result<()> {
        let mut owned = HashMap::<&str, Decimal>::new();
        for entry in self.entries.iter().filter(|entry| !entry.closed) {
            *owned.entry(&entry.token_id).or_default() += entry.filled_size.unwrap_or(entry.size);
        }
        anyhow::ensure!(
            owned.len() == positions.len(),
            "Paper position count differs from active source ledger count"
        );
        for (token, position) in positions {
            anyhow::ensure!(
                owned.get(token.as_str()) == Some(&position.size),
                "Paper shares differ from source ledger for token {token}"
            );
        }
        Ok(())
    }

    /// Create an empty, in-memory-only ledger with no disk persistence.
    /// Suitable for unit tests.
    pub fn new_in_memory() -> Self {
        Self::default()
    }

    // -- Persistence (atomic) -- Gap 1 -------------------------------------

    /// Write the ledger to disk atomically via a `.tmp` sibling + rename.
    /// No-op if this is an in-memory-only ledger (path is empty).
    /// Logs a warning on failure but never panics.
    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            tracing::warn!("Failed to persist copy ledger: {e}");
        }
    }
    pub fn try_save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        if self.path.is_empty() {
            return Ok(());
        }
        let tmp = format!("{}.tmp", self.path);
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, &self.path)?;
        #[cfg(unix)]
        std::fs::File::open(
            std::path::Path::new(&self.path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }

    // -- Mutations ----------------------------------------------------------

    /// Record that we entered a new position.
    ///
    /// Record one source lot. Multiple sources may own lots for the same token.
    pub fn record_copy(
        &mut self,
        token_id: String,
        source_wallet: String,
        size: Decimal,
        entry_price: Decimal,
    ) {
        self.push_copy(token_id, source_wallet, size, entry_price, None, None);
        self.save();
    }

    /// Record an immediately filled paper BUY with a single durable ledger write.
    pub fn record_filled_copy(
        &mut self,
        token_id: String,
        source_wallet: String,
        size: Decimal,
        entry_price: Decimal,
    ) -> anyhow::Result<()> {
        self.push_copy(token_id, source_wallet, size, entry_price, Some(size), None);
        self.try_save()
    }

    /// Record a paper fill together with the leader's original dollar signal.
    pub fn record_filled_copy_with_source(
        &mut self,
        token_id: String,
        source_wallet: String,
        size: Decimal,
        entry_price: Decimal,
        source_notional_usd: Decimal,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            source_notional_usd > Decimal::ZERO,
            "Source notional must be positive"
        );
        self.push_copy(
            token_id,
            source_wallet,
            size,
            entry_price,
            Some(size),
            Some(source_notional_usd),
        );
        self.try_save()
    }

    fn push_copy(
        &mut self,
        token_id: String,
        source_wallet: String,
        size: Decimal,
        entry_price: Decimal,
        filled_size: Option<Decimal>,
        source_notional_usd: Option<Decimal>,
    ) {
        self.entries.push(CopyEntry {
            entry_order_id: None,
            token_id,
            source_wallet,
            size,
            filled_size,
            realized_shares: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            entry_price,
            source_notional_usd,
            copied_at: Utc::now(),
            closed: false,
            closed_at: None,
        });
    }

    /// Return this target's currently owned shares for a token.
    pub fn active_size_for_source(&self, token_id: &str, source_wallet: &str) -> Decimal {
        self.entries
            .iter()
            .filter(|e| !e.closed && e.token_id == token_id && e.source_wallet == source_wallet)
            .map(|e| e.filled_size.unwrap_or(e.size))
            .sum()
    }

    /// Return every source with active shares for a token, aggregated per source.
    pub fn active_sources_for_token(&self, token_id: &str) -> Vec<(String, Decimal)> {
        let mut sources: Vec<(String, Decimal)> = Vec::new();
        for entry in self
            .entries
            .iter()
            .filter(|e| !e.closed && e.token_id == token_id)
        {
            let size = entry.filled_size.unwrap_or(entry.size);
            if size <= Decimal::ZERO {
                continue;
            }
            if let Some((_, total)) = sources
                .iter_mut()
                .find(|(wallet, _)| wallet == &entry.source_wallet)
            {
                *total += size;
            } else {
                sources.push((entry.source_wallet.clone(), size));
            }
        }
        sources
    }

    /// Cost basis consumed by a source-specific FIFO sale, without mutation.
    pub fn sale_cost_basis(
        &self,
        token_id: &str,
        source_wallet: &str,
        shares: Decimal,
    ) -> Option<Decimal> {
        if shares <= Decimal::ZERO || self.active_size_for_source(token_id, source_wallet) < shares
        {
            return None;
        }
        let mut remaining = shares;
        let mut basis = Decimal::ZERO;
        for entry in self.entries.iter().filter(|entry| {
            !entry.closed && entry.token_id == token_id && entry.source_wallet == source_wallet
        }) {
            let owned = entry.filled_size.unwrap_or(entry.size);
            let consumed = owned.min(remaining);
            basis += consumed * entry.entry_price;
            remaining -= consumed;
            if remaining <= Decimal::ZERO {
                break;
            }
        }
        (remaining <= Decimal::ZERO).then_some(basis)
    }

    /// Attribute sale proceeds and fees to this source's FIFO lots.
    pub fn record_source_sale(
        &mut self,
        token_id: &str,
        source_wallet: &str,
        shares: Decimal,
        price: Decimal,
        fee: Decimal,
    ) -> anyhow::Result<Decimal> {
        anyhow::ensure!(
            price >= Decimal::ZERO && fee >= Decimal::ZERO,
            "invalid sale price or fee"
        );
        let basis = self
            .sale_cost_basis(token_id, source_wallet, shares)
            .ok_or_else(|| anyhow::anyhow!("source lots do not cover requested sale"))?;
        let mut remaining = shares;
        let now = Utc::now();
        for entry in &mut self.entries {
            if remaining <= Decimal::ZERO {
                break;
            }
            if entry.closed || entry.token_id != token_id || entry.source_wallet != source_wallet {
                continue;
            }
            let owned = entry.filled_size.unwrap_or(entry.size);
            let consumed = owned.min(remaining);
            let allocated_fee = fee * consumed / shares;
            entry.realized_shares += consumed;
            entry.realized_pnl += consumed * price - allocated_fee - consumed * entry.entry_price;
            let left = owned - consumed;
            entry.filled_size = Some(left);
            remaining -= consumed;
            if left <= Decimal::ZERO {
                entry.closed = true;
                entry.closed_at = Some(now);
            }
        }
        anyhow::ensure!(
            remaining <= Decimal::ZERO,
            "source sale accounting incomplete"
        );
        self.try_save()?;
        Ok(basis)
    }

    /// Attribute an authoritative binary-market payout to every source lot.
    pub fn settle_token(&mut self, token_id: &str, payout: Decimal) -> anyhow::Result<()> {
        anyhow::ensure!(
            payout == Decimal::ZERO || payout == Decimal::ONE,
            "invalid binary payout"
        );
        let now = Utc::now();
        for entry in &mut self.entries {
            if entry.closed || entry.token_id != token_id {
                continue;
            }
            let shares = entry.filled_size.unwrap_or(entry.size);
            if shares <= Decimal::ZERO {
                continue;
            }
            entry.realized_shares += shares;
            entry.realized_pnl += shares * (payout - entry.entry_price);
            entry.filled_size = Some(Decimal::ZERO);
            entry.closed = true;
            entry.closed_at = Some(now);
        }
        Ok(())
    }

    /// Weighted average basis for the active lots owned by one target.
    pub fn average_price_for_source(&self, token_id: &str, source_wallet: &str) -> Option<Decimal> {
        let mut shares = Decimal::ZERO;
        let mut basis = Decimal::ZERO;
        for entry in self
            .entries
            .iter()
            .filter(|e| !e.closed && e.token_id == token_id && e.source_wallet == source_wallet)
        {
            let size = entry.filled_size.unwrap_or(entry.size);
            shares += size;
            basis += size * entry.entry_price;
        }
        if shares > Decimal::ZERO {
            Some(basis / shares)
        } else {
            None
        }
    }

    /// Consume shares from one source's lots, oldest first. Returns shares consumed.
    pub fn consume_source(
        &mut self,
        token_id: &str,
        source_wallet: &str,
        shares: Decimal,
    ) -> Decimal {
        let mut remaining = shares.max(Decimal::ZERO);
        let now = Utc::now();
        for entry in &mut self.entries {
            if remaining <= Decimal::ZERO {
                break;
            }
            if entry.closed || entry.token_id != token_id || entry.source_wallet != source_wallet {
                continue;
            }
            let owned = entry.filled_size.unwrap_or(entry.size);
            let consumed = owned.min(remaining);
            let left = owned - consumed;
            remaining -= consumed;
            entry.filled_size = Some(left);
            if left <= Decimal::ZERO {
                entry.closed = true;
                entry.closed_at = Some(now);
            }
        }
        let consumed = shares.max(Decimal::ZERO) - remaining;
        if consumed > Decimal::ZERO {
            self.save();
        }
        consumed
    }

    /// Mark every active lot for `(token_id, source_wallet)` as closed.
    pub fn record_close(&mut self, token_id: &str, source_wallet: &str) {
        let now = Utc::now();
        let mut changed = false;
        for entry in &mut self.entries {
            if !entry.closed && entry.token_id == token_id && entry.source_wallet == source_wallet {
                entry.closed = true;
                entry.closed_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save();
            return;
        }
        tracing::warn!(
            "record_close: no active entry found for token {} from {}",
            &token_id[..token_id.len().min(12)],
            &source_wallet[..source_wallet.len().min(10)],
        );
    }

    /// Close every source lot for a token after confirmed market settlement.
    pub fn record_token_close(&mut self, token_id: &str) {
        let now = Utc::now();
        let mut changed = false;
        for entry in &mut self.entries {
            if !entry.closed && entry.token_id == token_id {
                entry.closed = true;
                entry.closed_at = Some(now);
                entry.filled_size = Some(Decimal::ZERO);
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// Update the filled size for the most recent active entry for `token_id`. (Gap 4)
    ///
    /// Called by the position-sync task when it observes a position size that
    /// differs from the recorded order size.
    pub fn update_fill(&mut self, token_id: &str, actual_size: Decimal) {
        for entry in self.entries.iter_mut().rev() {
            if !entry.closed && entry.token_id == token_id {
                entry.filled_size = Some(actual_size);
                self.save();
                return;
            }
        }
    }

    // -- Queries ------------------------------------------------------------

    /// Whether we have **any** active (unclosed) copy for `token_id`, regardless of
    /// which target wallet it came from.
    ///
    /// Used by the BUY gate to enforce the one-position-per-token rule.
    pub fn has_any_active(&self, token_id: &str) -> bool {
        self.find_active_for_token(token_id).is_some()
    }

    /// Return the active entry for `token_id`, if one exists.
    ///
    /// Because of the one-position-per-token rule there can be at most one.
    pub fn find_active_for_token(&self, token_id: &str) -> Option<&CopyEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| !e.closed && e.token_id == token_id)
    }

    /// Whether we have an active copy for `token_id` specifically from `source_wallet`.
    pub fn has_active_copy(&self, token_id: &str, source_wallet: &str) -> bool {
        self.entries
            .iter()
            .rev()
            .any(|e| !e.closed && e.token_id == token_id && e.source_wallet == source_wallet)
    }

    // -- Startup reconciliation --------------------------------------------

    /// Close any ledger entries for tokens we no longer hold in our live wallet.
    ///
    /// Call this once at startup, after seeding `state.positions` from the Polymarket
    /// API.  It corrects for positions that were closed while the bot was offline.
    pub fn reconcile(&mut self, live_our_token_ids: &HashSet<String>) {
        let now = Utc::now();
        let mut changed = false;

        for entry in self.entries.iter_mut() {
            if entry.closed {
                continue;
            }
            if !live_our_token_ids.contains(&entry.token_id) {
                tracing::warn!(
                    "Ledger reconcile: token {} (from {}) no longer in wallet — marking closed.",
                    &entry.token_id[..entry.token_id.len().min(12)],
                    &entry.source_wallet[..entry.source_wallet.len().min(10)],
                );
                entry.closed = true;
                entry.closed_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save();
        } else {
            tracing::info!("Ledger reconcile: all active entries match live wallet. No changes.");
        }
    }

    // -- Pruning (Gap 11) --------------------------------------------------

    /// Remove closed entries whose `closed_at` timestamp is older than `days` UTC days.
    ///
    /// Active (unclosed) entries are never removed.
    /// Returns the number of entries pruned.
    pub fn prune_closed_older_than(&mut self, days: u32) -> usize {
        if days == 0 {
            return 0; // 0 = disabled
        }
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(days));
        let before = self.entries.len();
        self.entries.retain(|e| {
            if !e.closed {
                return true; // never prune active entries
            }
            // Keep if closed_at is recent or unknown
            e.closed_at.is_none_or(|t| t >= cutoff)
        });
        let pruned = before - self.entries.len();
        if pruned > 0 {
            tracing::info!(
                "Ledger: pruned {} closed entries older than {} days.",
                pruned,
                days
            );
            self.save();
        }
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    #[test]
    fn multiple_sources_keep_separate_lots_and_partial_sell_consumes_only_matching_source() {
        let mut ledger = CopyLedger::new_in_memory();
        ledger.record_copy("token".into(), "wallet-a".into(), d("10"), d("0.40"));
        ledger.entries.last_mut().unwrap().filled_size = Some(d("10"));
        ledger.record_copy("token".into(), "wallet-b".into(), d("5"), d("0.60"));
        ledger.entries.last_mut().unwrap().filled_size = Some(d("5"));

        assert_eq!(ledger.active_size_for_source("token", "wallet-a"), d("10"));
        assert_eq!(
            ledger.average_price_for_source("token", "wallet-b"),
            Some(d("0.60"))
        );
        assert_eq!(ledger.consume_source("token", "wallet-a", d("4")), d("4"));
        assert_eq!(ledger.active_size_for_source("token", "wallet-a"), d("6"));
        assert_eq!(ledger.active_size_for_source("token", "wallet-b"), d("5"));
        assert_eq!(ledger.consume_source("token", "wallet-a", d("6")), d("6"));
        assert_eq!(
            ledger.active_size_for_source("token", "wallet-a"),
            Decimal::ZERO
        );
        assert_eq!(ledger.active_size_for_source("token", "wallet-b"), d("5"));
    }

    #[test]
    fn sale_and_binary_settlement_realize_each_sources_own_cost_basis() {
        let mut ledger = CopyLedger::new_in_memory();
        ledger.record_copy("token".into(), "wallet-a".into(), d("10"), d("0.40"));
        ledger.entries.last_mut().unwrap().filled_size = Some(d("10"));
        ledger.record_copy("token".into(), "wallet-b".into(), d("5"), d("0.60"));
        ledger.entries.last_mut().unwrap().filled_size = Some(d("5"));

        assert_eq!(
            ledger.sale_cost_basis("token", "wallet-a", d("5")),
            Some(d("2.00"))
        );
        ledger
            .record_source_sale("token", "wallet-a", d("5"), d("0.50"), d("0.10"))
            .unwrap();
        assert_eq!(ledger.entries[0].realized_pnl, d("0.40"));
        assert_eq!(ledger.active_size_for_source("token", "wallet-a"), d("5"));
        assert_eq!(ledger.active_size_for_source("token", "wallet-b"), d("5"));
        assert_eq!(
            ledger.active_sources_for_token("token"),
            vec![
                ("wallet-a".to_string(), d("5")),
                ("wallet-b".to_string(), d("5"))
            ]
        );

        ledger.settle_token("token", Decimal::ONE).unwrap();
        assert_eq!(ledger.entries[0].realized_pnl, d("3.40"));
        assert_eq!(ledger.entries[1].realized_pnl, d("2.00"));
        assert!(ledger.entries.iter().all(|entry| entry.closed));
        assert!(!ledger.entries[0].closed_at.is_none());
    }
}
