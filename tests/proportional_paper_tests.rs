mod common;
use common::*;
use polycopier::{
    clients::OrderSubmitter,
    config::SizingMode,
    copy_ledger::CopyLedger,
    models::{OrderRequest, TradeSide},
    risk::RiskEngine,
    state::BotState,
    strategy::*,
};
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, RwLock};

#[tokio::test]
async fn every_paper_fill_scales_shares_and_preserves_aggregate_basis() {
    let mut cfg = test_config();
    cfg.is_sim = true;
    cfg.sizing_mode = SizingMode::TargetScalar;
    cfg.max_trade_size_usd = dec!(1000000000);
    cfg.max_delay_seconds = 30;
    cfg.target_scalars.insert("0xabc".into(), dec!(0.5));
    let state = Arc::new(RwLock::new(BotState::default()));
    state.write().await.total_balance = dec!(1000);
    let ledger = Arc::new(tokio::sync::Mutex::new(CopyLedger::default()));
    let log = Arc::new(Mutex::new(Vec::<OrderRequest>::new()));
    let capture = log.clone();
    let submitter: OrderSubmitter = Arc::new(move |order| {
        let capture = capture.clone();
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            capture.lock().unwrap().push(order);
            Ok(())
        })
    });
    let (tx, rx) = mpsc::channel(16);
    start_strategy_engine_with_fees(
        rx,
        state.clone(),
        RiskEngine::new(cfg.clone()),
        submitter,
        cfg,
        ledger.clone(),
        make_no_op_holds_query(),
        make_no_op_end_date_query(),
        Arc::new(|_| Box::pin(async { Ok(dec!(0)) })),
    );
    // Burst additions must neither debounce away fills nor overwrite inventory.
    // Tiny paper fills are included even when below the live CLOB minimum.
    for (size, side) in [
        (dec!(20), TradeSide::BUY),
        (dec!(10), TradeSide::BUY),
        (dec!(4), TradeSide::SELL),
        (dec!(2), TradeSide::BUY),
        (dec!(100), TradeSide::SELL),
    ] {
        tx.send(make_trade("0xabc", dec!(0.50), size, side))
            .await
            .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if log.lock().unwrap().len() == 5 && state.read().await.in_flight_tokens.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    {
        let orders = log.lock().unwrap();
        assert_eq!(
            orders.iter().map(|o| o.size).collect::<Vec<_>>(),
            vec![dec!(10), dec!(5), dec!(2), dec!(1), dec!(14)]
        );
    }
    let guard = state.read().await;
    assert!(guard.positions.is_empty());
    assert_eq!(guard.realized_pnl, dec!(-0.32));
    assert_eq!(guard.total_balance, dec!(999.68));
    assert_eq!(guard.paper_outcomes.len(), 1);
    assert_eq!(guard.paper_outcomes[0].1, dec!(-0.32));
    assert!(ledger.lock().await.entries[0].closed);
    assert_eq!(ledger.lock().await.entries.len(), 1);
}
