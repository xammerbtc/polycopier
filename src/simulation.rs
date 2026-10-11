use crate::{models::Position, state::BotState};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    positions: HashMap<String, Position>,
    total_balance: Decimal,
    realized_pnl: Decimal,
}
pub fn restore(state: &mut BotState) -> anyhow::Result<()> {
    match std::fs::read_to_string("simulation_state.json") {
        Ok(raw) => {
            let s: Snapshot = serde_json::from_str(&raw)?;
            state.positions = s.positions;
            state.total_balance = s.total_balance;
            state.realized_pnl = s.realized_pnl;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
pub fn save(state: &BotState) -> anyhow::Result<()> {
    let s = Snapshot {
        positions: state.positions.clone(),
        total_balance: state.total_balance,
        realized_pnl: state.realized_pnl,
    };
    std::fs::write("simulation_state.json.tmp", serde_json::to_vec_pretty(&s)?)?;
    std::fs::rename("simulation_state.json.tmp", "simulation_state.json")?;
    Ok(())
}
