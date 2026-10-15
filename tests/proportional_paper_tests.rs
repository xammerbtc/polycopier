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
    start_strategy_engine_with_queries(
        rx,
        state.clone(),
        RiskEngine::new(cfg.clone()),
        submitter,
        cfg,
        ledger.clone(),
        make_no_op_holds_query(),
        make_no_op_end_date_query(),
        Arc::new(|_| Box::pin(async { Ok(dec!(0)) })),
        Arc::new(|_token, shares, floor| {
            Box::pin(async move {
                // This synthetic sweep has no executable book; ordinary source
                // sells see bids at the price used by the original accounting test.
                Ok(floor.and_then(|floor| {
                    polycopier::simulation::match_sell_bids(
                        &[(dec!(0.49), dec!(1000))],
                        shares,
                        Some(floor),
                    )
                }))
            })
        }),
    );
    // Burst additions must neither debounce away fills nor overwrite inventory.
    // Proportional paper buys below the live CLOB minimum must be skipped too.
    let mut artificial = make_trade("0xabc", dec!(0.01), dec!(500), TradeSide::SELL);
    artificial.transaction_hash = "sweep_99999".into();
    tx.send(make_trade("0xabc", dec!(0.50), dec!(20), TradeSide::BUY))
        .await
        .unwrap();
    // A replay of the first signal waits for its pending fill and then sees
    // the committed key, just as a recovered snapshot does after restart.
    tx.send(make_trade("0xabc", dec!(0.50), dec!(20), TradeSide::BUY))
        .await
        .unwrap();
    tx.send(artificial).await.unwrap();
    for (size, side) in [
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
            if log.lock().unwrap().len() == 4 && state.read().await.in_flight_tokens.is_empty() {
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
            vec![dec!(10), dec!(5), dec!(2), dec!(13)]
        );
    }
    let guard = state.read().await;
    assert!(guard.positions.is_empty());
    assert_eq!(guard.realized_pnl, dec!(-0.30));
    assert_eq!(guard.total_balance, dec!(999.70));
    assert_eq!(guard.paper_outcomes.len(), 1);
    assert_eq!(guard.paper_outcomes[0].1, dec!(-0.30));
    assert!(guard.live_feed.iter().any(|event| event
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("order minimum")
            || reason.contains("below Polymarket minimum"))));
    let ledger = ledger.lock().await;
    assert_eq!(
        ledger.entries.len(),
        2,
        "each copied BUY should remain attributable as its own source lot"
    );
    assert!(ledger.entries.iter().all(|entry| entry.closed));
}

#[tokio::test]
async fn capped_paper_buys_keep_independent_source_lots() {
    let mut cfg = test_config();
    cfg.is_sim = true;
    // Keep this source-attribution test independent of the live CLOB book.
    // Fixed-dollar book sweeping is covered by the budget matcher tests.
    cfg.sizing_mode = SizingMode::TargetUsd;
    cfg.max_trade_size_usd = dec!(5);
    cfg.max_delay_seconds = 30;
    cfg.target_wallets.push("0xdef".into());
    cfg.target_scalars.insert("0xabc".into(), dec!(0.2));
    cfg.target_scalars.insert("0xdef".into(), dec!(0.5));
    let state = Arc::new(RwLock::new(BotState::default()));
    state.write().await.total_balance = dec!(1000);
    let ledger = Arc::new(tokio::sync::Mutex::new(CopyLedger::default()));
    let log = Arc::new(Mutex::new(Vec::<OrderRequest>::new()));
    let capture = log.clone();
    let submitter: OrderSubmitter = Arc::new(move |order| {
        let capture = capture.clone();
        Box::pin(async move {
            capture.lock().unwrap().push(order);
            Ok(())
        })
    });
    let (tx, rx) = mpsc::channel(8);
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
    let mut first = make_trade("0xabc", dec!(0.50), dec!(20), TradeSide::BUY);
    first.transaction_hash = "tx_first".into();
    let mut second = make_trade("0xdef", dec!(0.50), dec!(20), TradeSide::BUY);
    second.transaction_hash = "tx_second".into();
    tx.send(first).await.unwrap();
    tx.send(second).await.unwrap();

    let wait = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if ledger.lock().await.entries.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if wait.is_err() {
        panic!(
            "expected two source lots; copies={:?}, skipped={:?}",
            state.read().await.live_feed,
            state.read().await.rejection_reasons
        );
    }

    {
        let orders = log.lock().unwrap();
        assert_eq!(orders.len(), 2);
        assert!(orders
            .iter()
            .all(|order| order.size * order.price <= dec!(5)));
    }
    let ledger = ledger.lock().await;
    assert_eq!(ledger.entries.len(), 2);
    assert_ne!(
        ledger.entries[0].source_wallet,
        ledger.entries[1].source_wallet
    );
    assert!(ledger.entries.iter().all(|entry| entry.token_id == "99999"));
}
