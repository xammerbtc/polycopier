use crate::{
    models::{EvaluatedTrade, Position},
    state::BotState,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PAPER_BOOK_URL: &str = "https://clob.polymarket.com/book";
static PAPER_BOOK_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
static PAPER_LIQUIDITY: std::sync::OnceLock<std::sync::Mutex<HashMap<String, ReservedLiquidity>>> =
    std::sync::OnceLock::new();
static NEXT_PAPER_RESERVATION: AtomicU64 = AtomicU64::new(1);
const PAPER_LIQUIDITY_WINDOW: Duration = Duration::from_secs(10);

struct ReservedLiquidity {
    bought: Vec<ReservedLevel>,
    sold: Vec<ReservedLevel>,
}

struct ReservedLevel {
    id: u64,
    price: Decimal,
    shares: Decimal,
    at: Instant,
}

#[derive(Debug)]
struct ReservationHandle {
    token_id: String,
    buy: bool,
    id: u64,
    committed: bool,
}

impl Drop for ReservationHandle {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(reservations) = PAPER_LIQUIDITY.get() else {
            return;
        };
        let mut reservations = reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(token) = reservations.get_mut(&self.token_id) {
            let levels = if self.buy {
                &mut token.bought
            } else {
                &mut token.sold
            };
            levels.retain(|level| level.id != self.id);
        }
    }
}

/// Public books do not reflect our simulated fills. Reserve the displayed
/// liquidity briefly so concurrent paper signals cannot reuse the same levels.
fn match_with_reservation(
    token_id: &str,
    buy: bool,
    levels: &[(Decimal, Decimal)],
    matcher: impl FnOnce(&[(Decimal, Decimal)]) -> Option<PaperBuyFill>,
) -> Option<PaperBuyFill> {
    match_with_reservation_at(token_id, buy, levels, matcher, Instant::now())
}

fn match_with_reservation_at(
    token_id: &str,
    buy: bool,
    levels: &[(Decimal, Decimal)],
    matcher: impl FnOnce(&[(Decimal, Decimal)]) -> Option<PaperBuyFill>,
    now: Instant,
) -> Option<PaperBuyFill> {
    let mut reservations = PAPER_LIQUIDITY
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reservations.len() > 5000 {
        reservations.retain(|_, value| {
            value
                .bought
                .retain(|level| now.saturating_duration_since(level.at) < PAPER_LIQUIDITY_WINDOW);
            value
                .sold
                .retain(|level| now.saturating_duration_since(level.at) < PAPER_LIQUIDITY_WINDOW);
            !value.bought.is_empty() || !value.sold.is_empty()
        });
    }
    let reservation = reservations
        .entry(token_id.to_owned())
        .or_insert(ReservedLiquidity {
            bought: Vec::new(),
            sold: Vec::new(),
        });
    let reserved_levels = if buy {
        &mut reservation.bought
    } else {
        &mut reservation.sold
    };
    reserved_levels
        .retain(|level| now.saturating_duration_since(level.at) < PAPER_LIQUIDITY_WINDOW);
    let mut remaining_levels = levels.to_vec();
    remaining_levels.sort_by_key(|(price, _)| if buy { *price } else { -*price });
    let mut reserved_by_price = HashMap::<Decimal, Decimal>::new();
    for level in reserved_levels.iter() {
        *reserved_by_price.entry(level.price).or_default() += level.shares;
    }
    for (price, available) in &mut remaining_levels {
        let reserved = reserved_by_price.entry(*price).or_default();
        let removed = (*available).min(*reserved).max(Decimal::ZERO);
        *available -= removed;
        *reserved -= removed;
    }
    let mut fill = matcher(&remaining_levels)?;
    let id = NEXT_PAPER_RESERVATION.fetch_add(1, Ordering::Relaxed);
    let mut unallocated = fill.shares;
    for (price, available) in remaining_levels {
        if price <= Decimal::ZERO || available <= Decimal::ZERO {
            continue;
        }
        let shares = available.min(unallocated);
        reserved_levels.push(ReservedLevel {
            id,
            price,
            shares,
            at: now,
        });
        unallocated -= shares;
        if unallocated <= Decimal::ZERO {
            break;
        }
    }
    fill.reservation = Some(ReservationHandle {
        token_id: token_id.to_owned(),
        buy,
        id,
        committed: false,
    });
    Some(fill)
}

async fn fetch_paper_book(token_id: &str) -> anyhow::Result<serde_json::Value> {
    let client = PAPER_BOOK_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(3))
            .tcp_nodelay(true)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("paper order-book client")
    });
    let response = client
        .get(PAPER_BOOK_URL)
        .query(&[("token_id", token_id)])
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!(
            "no CLOB order book for this token (HTTP 404); market may be closed or token not tradable"
        );
    }
    Ok(response.error_for_status()?.json().await?)
}

#[derive(Debug)]
pub struct PaperBuyFill {
    pub shares: Decimal,
    pub average_price: Decimal,
    reservation: Option<ReservationHandle>,
}

impl PartialEq for PaperBuyFill {
    fn eq(&self, other: &Self) -> bool {
        self.shares == other.shares && self.average_price == other.average_price
    }
}

impl PaperBuyFill {
    pub fn commit_reservation(&mut self) {
        if let Some(reservation) = &mut self.reservation {
            reservation.committed = true;
        }
    }
}

/// Simulate a price-capped FAK buy by consuming the current public ask book.
/// Any quantity unavailable below `max_price` is canceled, as it would be for FAK.
pub async fn paper_buy_ladder(
    token_id: &str,
    requested_shares: Decimal,
    max_price: Decimal,
) -> anyhow::Result<Option<PaperBuyFill>> {
    anyhow::ensure!(
        requested_shares > Decimal::ZERO,
        "requested shares must be positive"
    );
    anyhow::ensure!(
        (Decimal::new(1, 2)..=Decimal::new(99, 2)).contains(&max_price),
        "max price must be between 0.01 and 0.99"
    );

    let book = fetch_paper_book(token_id).await?;

    let asks = book["asks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("order book response has no asks"))?;
    let mut levels = Vec::with_capacity(asks.len());
    for level in asks {
        let price = level["price"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book ask has no price"))?;
        let size = level["size"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book ask has no size"))?;
        levels.push((Decimal::from_str(price)?, Decimal::from_str(size)?));
    }
    Ok(match_with_reservation(
        token_id,
        true,
        &levels,
        |available| match_buy_asks(available, requested_shares, max_price),
    ))
}

/// Spend up to `budget_usd` against the cheapest available asks. The fee is
/// included in the budget, and unfilled budget is left in cash.
pub async fn paper_buy_budget_ladder(
    token_id: &str,
    budget_usd: Decimal,
    max_price: Decimal,
    fee_rate: Decimal,
) -> anyhow::Result<Option<PaperBuyFill>> {
    anyhow::ensure!(budget_usd > Decimal::ZERO, "budget must be positive");
    anyhow::ensure!(
        (Decimal::new(1, 2)..=Decimal::new(99, 2)).contains(&max_price),
        "max price must be between 0.01 and 0.99"
    );
    anyhow::ensure!(
        (Decimal::ZERO..=Decimal::ONE).contains(&fee_rate),
        "invalid fee rate"
    );
    let book = fetch_paper_book(token_id).await?;
    let asks = book["asks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("order book response has no asks"))?;
    let mut levels = Vec::with_capacity(asks.len());
    for level in asks {
        let price = level["price"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book ask has no price"))?;
        let size = level["size"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book ask has no size"))?;
        levels.push((Decimal::from_str(price)?, Decimal::from_str(size)?));
    }
    Ok(match_with_reservation(
        token_id,
        true,
        &levels,
        |available| match_buy_asks_with_budget(available, budget_usd, max_price, fee_rate),
    ))
}

pub fn match_buy_asks_with_budget(
    asks: &[(Decimal, Decimal)],
    budget_usd: Decimal,
    max_price: Decimal,
    fee_rate: Decimal,
) -> Option<PaperBuyFill> {
    if budget_usd <= Decimal::ZERO || fee_rate < Decimal::ZERO || fee_rate > Decimal::ONE {
        return None;
    }
    let mut levels = asks.to_vec();
    levels.sort_by_key(|level| level.0);
    let mut shares = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    let cent = Decimal::new(1, 2);
    for (price, available) in levels {
        if price > max_price {
            break;
        }
        if price <= Decimal::ZERO || available <= Decimal::ZERO {
            continue;
        }
        let candidate = available
            .min((budget_usd - notional) / price)
            .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
        if candidate <= Decimal::ZERO {
            break;
        }
        let affordable = |quantity: Decimal| {
            let next_shares = shares + quantity;
            let next_notional = notional + quantity * price;
            let average = next_notional / next_shares;
            // Paper accounting debits shares * the stored average price. The
            // decimal division can round up by a last unit, so test that exact
            // expression here rather than the unrounded book notional.
            next_shares * average + crate::fees::taker_fee(next_shares, average, fee_rate)
                <= budget_usd
        };
        let take = if affordable(candidate) {
            candidate
        } else {
            let mut low = Decimal::ZERO;
            let mut high = candidate;
            while high - low >= cent {
                let mid = ((low + high) / Decimal::from(2))
                    .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
                if mid <= low {
                    break;
                }
                if affordable(mid) {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            low
        };
        if take <= Decimal::ZERO {
            break;
        }
        shares += take;
        notional += take * price;
        if take < available {
            break;
        }
    }
    (shares > Decimal::ZERO).then(|| PaperBuyFill {
        shares,
        average_price: notional / shares,
        reservation: None,
    })
}

/// Simulate an immediate paper SELL by consuming public bid liquidity. `min_price`
/// applies a limit floor; `None` models a market order and accepts any positive bid.
pub async fn paper_sell_ladder(
    token_id: &str,
    requested_shares: Decimal,
    min_price: Option<Decimal>,
) -> anyhow::Result<Option<PaperBuyFill>> {
    anyhow::ensure!(
        requested_shares > Decimal::ZERO,
        "requested shares must be positive"
    );
    if let Some(price) = min_price {
        anyhow::ensure!(
            (Decimal::new(1, 2)..=Decimal::new(99, 2)).contains(&price),
            "minimum price must be between 0.01 and 0.99"
        );
    }
    let book = fetch_paper_book(token_id).await?;
    let bids = book["bids"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("order book response has no bids"))?;
    let mut levels = Vec::with_capacity(bids.len());
    for level in bids {
        let price = level["price"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book bid has no price"))?;
        let size = level["size"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("order book bid has no size"))?;
        levels.push((Decimal::from_str(price)?, Decimal::from_str(size)?));
    }
    Ok(match_with_reservation(
        token_id,
        false,
        &levels,
        |available| match_sell_bids(available, requested_shares, min_price),
    ))
}

/// Walk bids best-first and stop below the limit floor, if one is supplied.
pub fn match_sell_bids(
    bids: &[(Decimal, Decimal)],
    requested_shares: Decimal,
    min_price: Option<Decimal>,
) -> Option<PaperBuyFill> {
    if requested_shares <= Decimal::ZERO {
        return None;
    }
    let mut levels = bids.to_vec();
    levels.sort_by_key(|level| std::cmp::Reverse(level.0));
    let mut remaining = requested_shares;
    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for (price, available) in levels {
        if min_price.is_some_and(|floor| price < floor) {
            break;
        }
        if price <= Decimal::ZERO || available <= Decimal::ZERO {
            continue;
        }
        let size = remaining.min(available);
        filled += size;
        notional += size * price;
        remaining -= size;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    if filled > Decimal::ZERO {
        Some(PaperBuyFill {
            shares: filled,
            average_price: notional / filled,
            reservation: None,
        })
    } else {
        None
    }
}

/// Walk asks cheapest-first; this is the paper equivalent of one FAK order
/// with a hard price cap, rather than several independently resting GTC orders.
pub fn match_buy_asks(
    asks: &[(Decimal, Decimal)],
    requested_shares: Decimal,
    max_price: Decimal,
) -> Option<PaperBuyFill> {
    if requested_shares <= Decimal::ZERO {
        return None;
    }
    let mut levels = asks.to_vec();
    levels.sort_by_key(|level| level.0);

    let mut remaining = requested_shares;
    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for (price, available) in levels {
        if price > max_price {
            break;
        }
        if price <= Decimal::ZERO || available <= Decimal::ZERO {
            continue;
        }
        let size = remaining.min(available);
        filled += size;
        notional += size * price;
        remaining -= size;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    if filled == Decimal::ZERO {
        None
    } else {
        Some(PaperBuyFill {
            shares: filled,
            average_price: notional / filled,
            reservation: None,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    processed_signals: HashMap<String, i64>,
    positions: HashMap<String, Position>,
    total_balance: Decimal,
    realized_pnl: Decimal,
    #[serde(default)]
    cycle_pnl: HashMap<String, Decimal>,
    #[serde(default)]
    outcomes: Vec<(String, Decimal, i64)>,
    #[serde(default)]
    copies_executed: u32,
    #[serde(default)]
    trades_skipped: u32,
    #[serde(default)]
    skip_attempts: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    live_feed: std::collections::VecDeque<EvaluatedTrade>,
}
fn apply_snapshot(state: &mut BotState, snapshot: Snapshot) {
    state.paper_processed_signals = snapshot.processed_signals;
    state.positions = snapshot.positions;
    state.total_balance = snapshot.total_balance;
    state.realized_pnl = snapshot.realized_pnl;
    state.paper_cycle_pnl = snapshot.cycle_pnl;
    state.paper_outcomes = snapshot.outcomes;
    state.paper_committed_outcomes = state.paper_outcomes.len();
    state.copies_executed = snapshot.copies_executed;
    state.trades_skipped = snapshot.trades_skipped;
    state.skip_attempts = snapshot.skip_attempts;
    state.live_feed = snapshot.live_feed;
}
pub fn restore(state: &mut BotState) -> anyhow::Result<()> {
    match std::fs::read_to_string("simulation_state.json") {
        Ok(raw) => {
            apply_snapshot(state, serde_json::from_str(&raw)?);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
pub fn save(state: &BotState) -> anyhow::Result<()> {
    let s = Snapshot {
        processed_signals: state.paper_processed_signals.clone(),
        positions: state.positions.clone(),
        total_balance: state.total_balance,
        realized_pnl: state.realized_pnl,
        cycle_pnl: state.paper_cycle_pnl.clone(),
        outcomes: state.paper_outcomes.clone(),
        copies_executed: state.copies_executed,
        trades_skipped: state.trades_skipped,
        skip_attempts: state.skip_attempts.clone(),
        live_feed: state.live_feed.clone(),
    };
    use std::io::Write;
    let mut file = std::fs::File::create("simulation_state.json.tmp")?;
    file.write_all(&serde_json::to_vec(&s)?)?;
    file.sync_all()?;
    std::fs::rename("simulation_state.json.tmp", "simulation_state.json")?;
    #[cfg(unix)]
    std::fs::File::open(".")?.sync_all()?;
    Ok(())
}

pub fn record_outcome(state: &mut BotState, token: &str, pnl: Decimal) {
    let id = format!("paper:{}:{token}", state.paper_outcomes.len());
    state
        .paper_outcomes
        .push((id, pnl, chrono::Utc::now().timestamp()));
}

pub fn commit(state: &mut BotState) -> anyhow::Result<()> {
    save(state)?;
    state.paper_committed_outcomes = state.paper_outcomes.len();
    Ok(())
}

pub fn sync_risk(risk: &mut crate::risk::RiskEngine, state: &BotState) -> anyhow::Result<()> {
    anyhow::ensure!(
        state.paper_committed_outcomes <= state.paper_outcomes.len(),
        "Invalid paper outcome checkpoint"
    );
    risk.sync_live_outcomes(&state.paper_outcomes[..state.paper_committed_outcomes])
}

/// Fill a paper SELL against owned inventory. Return a risk outcome only at full exit.
pub fn sell(
    state: &mut BotState,
    token: &str,
    shares: Decimal,
    price: Decimal,
    fee: Decimal,
) -> anyhow::Result<Option<Decimal>> {
    let position = state
        .positions
        .get(token)
        .ok_or_else(|| anyhow::anyhow!("Paper SELL has no owned position"))?;
    let cost_basis = shares * position.average_entry_price;
    sell_with_cost_basis(state, token, shares, price, fee, cost_basis)
}

/// Fill a source-specific paper exit using that source's FIFO lot basis.
pub fn sell_with_cost_basis(
    state: &mut BotState,
    token: &str,
    shares: Decimal,
    price: Decimal,
    fee: Decimal,
    cost_basis: Decimal,
) -> anyhow::Result<Option<Decimal>> {
    let position = state
        .positions
        .get_mut(token)
        .ok_or_else(|| anyhow::anyhow!("Paper SELL has no owned position"))?;
    anyhow::ensure!(
        shares > Decimal::ZERO
            && shares <= position.size
            && fee >= Decimal::ZERO
            && cost_basis >= Decimal::ZERO
            && cost_basis <= position.size * position.average_entry_price + Decimal::new(1, 8),
        "Invalid paper SELL quantity or fee"
    );
    let proceeds = shares * price - fee;
    let pnl = proceeds - cost_basis;
    let remaining_basis = position.size * position.average_entry_price - cost_basis;
    position.size -= shares;
    let closed = position.size == Decimal::ZERO;
    if !closed {
        position.average_entry_price = remaining_basis / position.size;
    }
    state.total_balance += proceeds;
    state.realized_pnl += pnl;
    *state.paper_cycle_pnl.entry(token.into()).or_default() += pnl;
    if closed {
        state.positions.remove(token);
        let outcome = state.paper_cycle_pnl.remove(token);
        if let Some(pnl) = outcome {
            record_outcome(state, token, pnl);
        }
        Ok(outcome)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn paper_buy_sweeps_asks_in_price_order_and_stops_at_cap() {
        let asks = vec![
            (d("0.90"), d("2")),
            (d("0.60"), d("4")),
            (d("0.75"), d("3")),
            (d("0.91"), d("100")),
        ];
        let fill = match_buy_asks(&asks, d("10"), d("0.90")).unwrap();
        assert_eq!(fill.shares, d("9"));
        assert_eq!(fill.average_price, d("0.7166666666666666666666666667"));
    }

    #[test]
    fn fixed_budget_sweeps_cheap_asks_and_includes_fees() {
        let asks = [
            (d("0.90"), d("100")),
            (d("0.99"), d("100")),
            (d("0.60"), d("3")),
        ];
        let fill = match_buy_asks_with_budget(&asks, d("5"), d("0.98"), d("0.07")).unwrap();
        assert!(fill.shares > d("5.10"), "cheap asks must increase shares");
        assert!(
            fill.shares < d("7"),
            "the five dollar cap must stop the sweep"
        );
        let cost = fill.shares * fill.average_price
            + crate::fees::taker_fee(fill.shares, fill.average_price, d("0.07"));
        assert!(cost <= d("5"), "fee-inclusive cost {cost} exceeds budget");
        assert!(
            match_buy_asks_with_budget(&[(d("0.99"), d("100"))], d("5"), d("0.98"), d("0.07"))
                .is_none()
        );
    }

    #[test]
    fn paper_buy_cancels_unfilled_remainder_when_no_asks_are_under_cap() {
        let asks = vec![(d("0.91"), d("100"))];
        assert_eq!(match_buy_asks(&asks, d("10"), d("0.90")), None);
    }

    #[test]
    fn paper_buy_budget_uses_actual_ask_prices_instead_of_price_cap() {
        let asks = vec![(d("0.98"), d("100")), (d("0.60"), d("100"))];
        let fill = match_buy_asks_with_budget(&asks, d("5"), d("0.98"), Decimal::ZERO).unwrap();
        assert_eq!(fill.shares, d("8.33"));
        assert_eq!(fill.shares * fill.average_price, d("4.998"));
    }

    #[test]
    fn paper_buy_budget_includes_fee_and_respects_price_cap() {
        let asks = vec![
            (d("0.60"), d("4")),
            (d("0.80"), d("100")),
            (d("0.99"), d("100")),
        ];
        let fill = match_buy_asks_with_budget(&asks, d("5"), d("0.98"), d("0.10")).unwrap();
        let cost = fill.shares * fill.average_price
            + crate::fees::taker_fee(fill.shares, fill.average_price, d("0.10"));
        assert!(cost <= d("5"));
        assert!(fill.shares > d("5.10"));
    }

    #[test]
    fn paper_budget_matches_the_amount_debited_from_cash() {
        for first in ["0.008", "0.013", "0.37", "0.61"] {
            for second in ["0.02", "0.17", "0.70", "0.97"] {
                let asks = [(d(first), d("8.65")), (d(second), d("1000"))];
                for budget in ["1", "2", "5"] {
                    for rate in ["0", "0.01", "0.07"] {
                        if let Some(fill) =
                            match_buy_asks_with_budget(&asks, d(budget), d("0.98"), d(rate))
                        {
                            let debit = fill.shares * fill.average_price
                                + crate::fees::taker_fee(fill.shares, fill.average_price, d(rate));
                            assert!(
                                debit <= d(budget),
                                "{first}, {second}, {budget}, {rate}: {debit}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paper_snapshot_keeps_counters_and_skip_deduplication_after_restart() {
        let skipped = EvaluatedTrade {
            original_event: crate::models::TradeEvent {
                transaction_hash: "0xpaper-skip".into(),
                maker_address: "0xsource".into(),
                taker_address: "0xsource".into(),
                token_id: "paper-token".into(),
                price: d("0.5"),
                size: d("10"),
                side: crate::models::TradeSide::BUY,
                timestamp: 123,
            },
            validated: false,
            reason: Some("No asks under price cap".into()),
        };
        let snapshot = Snapshot {
            processed_signals: HashMap::new(),
            positions: HashMap::new(),
            total_balance: d("20000"),
            realized_pnl: d("-4"),
            cycle_pnl: HashMap::new(),
            outcomes: Vec::new(),
            copies_executed: 17,
            trades_skipped: 3,
            skip_attempts: std::collections::BTreeMap::from([("no_liquidity".into(), 3)]),
            live_feed: std::collections::VecDeque::from([skipped.clone()]),
        };
        let persisted = serde_json::to_string(&snapshot).unwrap();
        let mut restored = BotState::new(true, None);
        apply_snapshot(&mut restored, serde_json::from_str(&persisted).unwrap());
        assert_eq!((restored.copies_executed, restored.trades_skipped), (17, 3));
        assert_eq!(restored.skip_attempts["no_liquidity"], 3);
        restored.push_evaluated_trade(skipped);
        assert_eq!(restored.trades_skipped, 3);

        let legacy = r#"{"positions":{},"total_balance":"20000","realized_pnl":"0"}"#;
        let old: Snapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!((old.copies_executed, old.trades_skipped), (0, 0));
        assert!(old.live_feed.is_empty());
        assert!(old.skip_attempts.is_empty());
    }

    #[test]
    fn paper_sell_sweeps_bids_and_respects_limit_floor() {
        let bids = vec![
            (d("0.50"), d("2")),
            (d("0.70"), d("3")),
            (d("0.60"), d("4")),
        ];
        let fill = match_sell_bids(&bids, d("6"), Some(d("0.55"))).unwrap();
        assert_eq!(fill.shares, d("6"));
        assert_eq!(fill.average_price, d("0.65"));
        assert_eq!(match_sell_bids(&bids, d("6"), Some(d("0.75"))), None);
    }

    #[test]
    fn rapid_paper_fills_cannot_reuse_the_same_displayed_book_shares() {
        let asks = [(d("0.40"), d("5")), (d("0.50"), d("5"))];
        let first = match_with_reservation("reservation-test-buy", true, &asks, |available| {
            match_buy_asks(available, d("7"), d("0.98"))
        })
        .unwrap();
        let second = match_with_reservation("reservation-test-buy", true, &asks, |available| {
            match_buy_asks(available, d("7"), d("0.98"))
        })
        .unwrap();
        assert_eq!(first.shares, d("7"));
        assert_eq!(second.shares, d("3"));
        assert_eq!(second.average_price, d("0.50"));

        let bids = [(d("0.60"), d("5")), (d("0.50"), d("5"))];
        let first = match_with_reservation("reservation-test-sell", false, &bids, |available| {
            match_sell_bids(available, d("7"), None)
        })
        .unwrap();
        let second = match_with_reservation("reservation-test-sell", false, &bids, |available| {
            match_sell_bids(available, d("7"), None)
        })
        .unwrap();
        assert_eq!(first.shares, d("7"));
        assert_eq!(second.shares, d("3"));
        assert_eq!(second.average_price, d("0.50"));
    }

    #[test]
    fn paper_reservations_follow_price_levels_and_expire_per_fill() {
        let start = Instant::now();
        let original = [(d("0.40"), d("5"))];
        let first = match_with_reservation_at(
            "reservation-test-levels",
            true,
            &original,
            |available| match_buy_asks(available, d("5"), d("0.98")),
            start,
        )
        .unwrap();
        assert_eq!(first.average_price, d("0.40"));

        // A newly appearing, better level must not be consumed by a reservation
        // made at a different price.
        let shifted = [(d("0.35"), d("5")), (d("0.40"), d("5"))];
        let second = match_with_reservation_at(
            "reservation-test-levels",
            true,
            &shifted,
            |available| match_buy_asks(available, d("5"), d("0.98")),
            start + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(second.average_price, d("0.35"));

        // The older reservation expires even though another fill was made
        // during its lifetime.
        let third = match_with_reservation_at(
            "reservation-test-levels",
            true,
            &shifted,
            |available| match_buy_asks(available, d("5"), d("0.98")),
            start + Duration::from_secs(11),
        )
        .unwrap();
        assert_eq!(third.average_price, d("0.40"));
    }

    #[test]
    fn rejected_paper_fill_releases_only_its_own_liquidity() {
        let asks = [(d("0.40"), d("5")), (d("0.50"), d("5"))];
        let first = match_with_reservation("reservation-test-rollback", true, &asks, |book| {
            match_buy_asks(book, d("5"), d("0.98"))
        })
        .unwrap();
        let mut second = match_with_reservation("reservation-test-rollback", true, &asks, |book| {
            match_buy_asks(book, d("5"), d("0.98"))
        })
        .unwrap();
        assert_eq!(second.average_price, d("0.50"));
        second.commit_reservation();
        drop(first); // Risk rejection must make the first price level available again.
        let third = match_with_reservation("reservation-test-rollback", true, &asks, |book| {
            match_buy_asks(book, d("5"), d("0.98"))
        })
        .unwrap();
        assert_eq!(third.average_price, d("0.40"));
    }

    #[test]
    fn partial_exit_keeps_fee_inclusive_basis_and_survives_restart() {
        let mut state = BotState::new(true, Some(d("193.86")));
        state.positions.insert(
            "token".into(),
            Position {
                token_id: "token".into(),
                size: d("10"),
                average_entry_price: d("0.614"),
            },
        );
        assert_eq!(
            sell(&mut state, "token", d("9"), d("0.6"), d("0.108")).unwrap(),
            None
        );
        assert_eq!(state.positions["token"].size, d("1"));
        assert_eq!(state.realized_pnl, d("-0.234"));
        let snapshot = Snapshot {
            processed_signals: state.paper_processed_signals.clone(),
            positions: state.positions.clone(),
            total_balance: state.total_balance,
            realized_pnl: state.realized_pnl,
            cycle_pnl: state.paper_cycle_pnl.clone(),
            outcomes: state.paper_outcomes.clone(),
            copies_executed: state.copies_executed,
            trades_skipped: state.trades_skipped,
            skip_attempts: state.skip_attempts.clone(),
            live_feed: state.live_feed.clone(),
        };
        let restored: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        state.positions = restored.positions;
        state.paper_cycle_pnl = restored.cycle_pnl;
        // A profitable last slice does not turn the overall losing cycle into a win.
        assert_eq!(
            sell(&mut state, "token", d("1"), d("0.7"), d("0.01")).unwrap(),
            Some(d("-0.158"))
        );
        assert_eq!(state.total_balance, d("199.842"));
        assert_eq!(state.realized_pnl, d("-0.158"));
        assert!(state.positions.is_empty());
        assert!(state.paper_cycle_pnl.is_empty());
        assert_eq!(state.paper_outcomes.len(), 1);
        assert_eq!(state.paper_outcomes[0].1, d("-0.158"));
        assert!(sell(&mut state, "token", d("1"), d("0.7"), Decimal::ZERO).is_err());
    }
    #[test]
    fn committed_signal_keys_roundtrip_with_inventory() {
        let raw = r#"{"positions":{},"total_balance":"10000","realized_pnl":"0","processed_signals":{"fill-key":123}}"#;
        let snapshot: Snapshot = serde_json::from_str(raw).unwrap();
        let restored: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(restored.processed_signals["fill-key"], 123);
        assert_eq!(restored.total_balance, d("10000"));
    }

    #[test]
    fn legacy_snapshot_and_oversell_are_safe() {
        let legacy: Snapshot =
            serde_json::from_str(r#"{"positions":{},"total_balance":"200","realized_pnl":"0"}"#)
                .unwrap();
        assert!(legacy.cycle_pnl.is_empty());
        assert!(legacy.outcomes.is_empty());
        let mut state = BotState::new(true, Some(d("200")));
        state.positions.insert(
            "token".into(),
            Position {
                token_id: "token".into(),
                size: d("1"),
                average_entry_price: d("0.5"),
            },
        );
        assert!(sell(&mut state, "token", d("2"), d("0.7"), Decimal::ZERO).is_err());
        assert_eq!(state.positions["token"].size, d("1"));
        assert_eq!(state.total_balance, d("200"));
    }
}
