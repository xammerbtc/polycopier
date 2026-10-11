mod common;
#[tokio::test]
async fn exact_order_ownership_and_uncertain_startup_protect_shared_wallet() {
    use polycopier::{
        copy_ledger::CopyLedger,
        execution_journal as journal,
        models::{OrderRequest, Position, TradeSide},
        risk::RiskEngine,
        state::BotState,
    };
    use rust_decimal_macros::dec;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{mpsc, Mutex, RwLock};
    let original = std::env::current_dir().unwrap();
    let directory =
        std::env::temp_dir().join(format!("polycopier-order-journal-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    std::env::set_current_dir(&directory).unwrap();
    journal::initialize().unwrap();
    let request = OrderRequest {
        token_id: "33333".into(),
        price: dec!(0.5),
        size: dec!(10),
        side: TradeSide::BUY,
    };
    let intent = journal::begin(&request).unwrap();
    assert!(
        journal::initialize().is_err(),
        "crashed in-flight submission cannot be silently retried"
    );
    journal::finish(&intent, "accepted", Some("our-order".into())).unwrap();
    assert!(journal::owned_ids().unwrap().contains("our-order"));
    assert!(!journal::owned_ids().unwrap().contains("donor-order"));
    let config = common::test_config();
    let mut initial = BotState::new(false, None);
    initial.total_balance = dec!(100);
    for token in ["99999", "33333"] {
        initial.positions.insert(
            token.into(),
            Position {
                token_id: token.into(),
                size: dec!(20),
                average_entry_price: dec!(0.5),
            },
        );
    }
    initial.foreign_order_tokens.insert("22222".into());
    let state = Arc::new(RwLock::new(initial));
    let ledger = Arc::new(Mutex::new(CopyLedger::new_in_memory()));
    ledger
        .lock()
        .await
        .record_copy("33333".into(), "0xabc".into(), dec!(20), dec!(0.5));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let submitter: polycopier::clients::OrderSubmitter = Arc::new(move |_| {
        let observed = observed.clone();
        Box::pin(async move {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    });
    let (tx, rx) = mpsc::channel(10);
    polycopier::strategy::start_strategy_engine(
        rx,
        state.clone(),
        RiskEngine::new(config.clone()),
        submitter,
        config,
        ledger.clone(),
        polycopier::strategy::make_no_op_holds_query(),
        polycopier::strategy::make_no_op_end_date_query(),
    );
    for (token, side) in [
        ("99999", TradeSide::SELL),
        ("22222", TradeSide::BUY),
        ("33333", TradeSide::SELL),
    ] {
        tx.send(common::make_trade_for_token(
            "0xabc",
            token,
            dec!(0.6),
            dec!(20),
            side,
        ))
        .await
        .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if calls.load(Ordering::Relaxed) == 1 && state.read().await.trades_skipped == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "only ledger-owned token reaches execution"
    );
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        ledger.lock().await.has_any_active("33333"),
        "order acceptance cannot close an unfilled live position"
    );
    assert_eq!(
        state.read().await.realized_pnl,
        dec!(0),
        "unfilled SELL cannot realize profit"
    );
    let rejected = journal::begin(&request).unwrap();
    journal::finish(&rejected, "rejected", None).unwrap();
    let uncertain = journal::begin(&request).unwrap();
    journal::uncertain(&uncertain);
    assert!(journal::blocked());
    assert!(journal::initialize().is_err());
    assert!(journal::begin(&request).is_err());
    std::env::set_current_dir(original).unwrap();
    assert!(directory
        .canonicalize()
        .unwrap()
        .starts_with(std::env::temp_dir().canonicalize().unwrap()));
    std::fs::remove_dir_all(directory).unwrap();
}
