use crate::{models::Position, state::BotState};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
}
pub fn restore(state: &mut BotState) -> anyhow::Result<()> {
    match std::fs::read_to_string("simulation_state.json") {
        Ok(raw) => {
            let s: Snapshot = serde_json::from_str(&raw)?;
            state.paper_processed_signals = s.processed_signals;
            state.positions = s.positions;
            state.total_balance = s.total_balance;
            state.realized_pnl = s.realized_pnl;
            state.paper_cycle_pnl = s.cycle_pnl;
            state.paper_outcomes = s.outcomes;
            state.paper_committed_outcomes = state.paper_outcomes.len();
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
    };
    use std::io::Write;
    let mut file = std::fs::File::create("simulation_state.json.tmp")?;
    file.write_all(&serde_json::to_vec_pretty(&s)?)?;
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
        .get_mut(token)
        .ok_or_else(|| anyhow::anyhow!("Paper SELL has no owned position"))?;
    anyhow::ensure!(
        shares > Decimal::ZERO && shares <= position.size && fee >= Decimal::ZERO,
        "Invalid paper SELL quantity or fee"
    );
    let proceeds = shares * price - fee;
    let pnl = proceeds - shares * position.average_entry_price;
    position.size -= shares;
    let closed = position.size == Decimal::ZERO;
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
