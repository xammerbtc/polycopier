//! Paper profit guard. It routes exits through the normal source-aware strategy
//! path, so book fills, fees, inventory and realized PnL share one accounting path.
use crate::{
    copy_ledger::CopyLedger,
    models::{TradeEvent, TradeSide},
    state::BotState,
};
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Mutex, RwLock};

pub fn eligible(
    equity: Decimal,
    starting_bank: Decimal,
    trigger: Decimal,
    mark: Decimal,
    basis: Decimal,
    loss: Decimal,
) -> bool {
    starting_bank > Decimal::ZERO
        && basis > Decimal::ZERO
        && equity >= starting_bank * (Decimal::ONE + trigger)
        && losing(mark, basis, loss)
}

fn losing(mark: Decimal, basis: Decimal, loss: Decimal) -> bool {
    basis > Decimal::ZERO && mark <= basis * (Decimal::ONE - loss)
}

fn load_arm(path: &Path, experiment: &str) -> bool {
    if experiment.is_empty() {
        return false;
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|value| {
            value["experiment"].as_str() == Some(experiment)
                && value["armed"].as_bool() == Some(true)
        })
}

fn store_arm(path: &Path, experiment: &str) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(
        serde_json::json!({"experiment":experiment,"armed":true})
            .to_string()
            .as_bytes(),
    )?;
    file.sync_all()?;
    std::fs::rename(temporary, path)
}

pub fn start_paper(
    state: Arc<RwLock<BotState>>,
    ledger: Arc<Mutex<CopyLedger>>,
    tx: mpsc::Sender<TradeEvent>,
    starting_bank: Decimal,
) {
    tokio::spawn(async move {
        let experiment = std::env::var("PAPER_EXPERIMENT_START_TIMESTAMP").unwrap_or_default();
        let arm_path = PathBuf::from(
            std::env::var("PAPER_PROFIT_GUARD_STATE_PATH")
                .unwrap_or_else(|_| "paper_profit_guard_armed.json".into()),
        );
        let mut sent = HashMap::<(String, String), Instant>::new();
        let mut armed = load_arm(&arm_path, &experiment);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let config = match crate::config::Config::reload() {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!("Profit guard configuration unavailable: {error}");
                    continue;
                }
            };
            let trigger = config.profit_guard_trigger_pct;
            let loss = config.profit_guard_loss_pct;
            if trigger <= Decimal::ZERO || loss <= Decimal::ZERO {
                if armed {
                    if let Err(error) = std::fs::remove_file(&arm_path) {
                        tracing::warn!("Profit guard arm marker could not be removed: {error}");
                    }
                }
                armed = false;
                continue;
            }
            let now = chrono::Utc::now().timestamp() as u64;
            let candidates = {
                let guard = state.read().await;
                if guard.started {
                    continue;
                }
                let mut equity = guard.total_balance;
                let mut open = Vec::new();
                for position in guard.positions.values() {
                    // Closed markets have no executable book. Their payouts are
                    // deliberately excluded until the settlement worker confirms them.
                    if guard
                        .position_details
                        .get(&position.token_id)
                        .is_some_and(|m| m.closed)
                    {
                        continue;
                    }
                    let fresh = guard
                        .price_updated_at
                        .get(&position.token_id)
                        .is_some_and(|at| now.saturating_sub(*at) <= 30);
                    let clob = guard
                        .position_details
                        .get(&position.token_id)
                        .is_some_and(|m| m.price_source == "CLOB midpoint");
                    // Missing marks contribute zero to the equity lower bound.
                    // This cannot create a false profit trigger and does not let
                    // one illiquid market disable exits for every other market.
                    if !fresh || !clob {
                        continue;
                    }
                    let Some(mark) = guard.position_prices.get(&position.token_id).copied() else {
                        continue;
                    };
                    equity += position.size * mark;
                    open.push((
                        position.token_id.clone(),
                        position.average_entry_price,
                        mark,
                    ));
                }
                if equity >= starting_bank * (Decimal::ONE + trigger) && !armed {
                    if experiment.is_empty() {
                        armed = true;
                    } else if let Err(error) = store_arm(&arm_path, &experiment) {
                        tracing::error!("Profit guard activation could not be persisted: {error}");
                    } else {
                        armed = true;
                    }
                }
                if !armed {
                    Vec::new()
                } else {
                    open.into_iter()
                        .filter(|(_, basis, mark)| losing(*mark, *basis, loss))
                        .filter(|(token, _, _)| {
                            !guard.in_flight_tokens.contains(token)
                                && !guard.pending_orders.contains_key(token)
                        })
                        .collect::<Vec<_>>()
                }
            };
            if candidates.is_empty() {
                continue;
            }
            let sources = {
                let ledger = ledger.lock().await;
                candidates
                    .into_iter()
                    .flat_map(|(token, _, mark)| {
                        let mut wallets: Vec<_> = ledger
                            .entries
                            .iter()
                            .filter(|entry| {
                                !entry.closed
                                    && entry.token_id == token
                                    && entry.filled_size.unwrap_or(entry.size) > Decimal::ZERO
                            })
                            .map(|entry| entry.source_wallet.clone())
                            .collect();
                        wallets.sort();
                        wallets.dedup();
                        wallets
                            .into_iter()
                            .map(move |wallet| (token.clone(), wallet, mark))
                    })
                    .collect::<Vec<_>>()
            };
            sent.retain(|_, at| at.elapsed() < Duration::from_secs(120));
            let mut emitted = 0;
            for (token, wallet, mark) in sources {
                if emitted >= 20 {
                    break;
                }
                let key = (token.clone(), wallet.clone());
                if sent
                    .get(&key)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
                {
                    continue;
                }
                let event = TradeEvent {
                    transaction_hash: format!("sweep_profit_{token}_{wallet}_{}", now / 30),
                    maker_address: String::new(),
                    taker_address: wallet.clone(),
                    token_id: token,
                    price: mark,
                    // The strategy's sweep branch derives exact remaining source
                    // inventory from the ledger; this field is not a share limit.
                    size: Decimal::ONE,
                    side: TradeSide::SELL,
                    timestamp: now as i64,
                };
                if tx.send(event).await.is_ok() {
                    sent.insert(key, Instant::now());
                    emitted += 1;
                } else {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    #[test]
    fn profit_and_loss_boundaries_are_strictly_account_based() {
        assert!(!eligible(
            dec!(10699),
            dec!(10000),
            dec!(0.07),
            dec!(0.80),
            dec!(1),
            dec!(0.10)
        ));
        assert!(eligible(
            dec!(10700),
            dec!(10000),
            dec!(0.07),
            dec!(0.90),
            dec!(1),
            dec!(0.10)
        ));
        assert!(!eligible(
            dec!(10700),
            dec!(10000),
            dec!(0.07),
            dec!(0.91),
            dec!(1),
            dec!(0.10)
        ));
        assert!(!eligible(
            dec!(10700),
            dec!(10000),
            dec!(0.07),
            dec!(0.80),
            dec!(0),
            dec!(0.10)
        ));
    }
    #[test]
    fn armed_state_is_scoped_to_the_experiment() {
        let path = std::env::temp_dir().join(format!("profit-guard-{}.json", std::process::id()));
        store_arm(&path, "experiment-one").unwrap();
        assert!(load_arm(&path, "experiment-one"));
        assert!(!load_arm(&path, "experiment-two"));
        std::fs::remove_file(path).unwrap();
    }
}
