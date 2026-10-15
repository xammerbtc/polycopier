mod common;
use polycopier::{
    copy_ledger::CopyLedger,
    execution_journal as journal,
    live_fills::{self, Fill},
    models::{OrderRequest, Position, TradeSide},
    risk::RiskEngine,
    state::BotState,
};
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[tokio::test]
async fn durable_fills_recover_owned_inventory_and_risk_without_double_counting() {
    let original = std::env::current_dir().unwrap();
    let dir = std::env::temp_dir().join(format!(
        "polycopier-settlement-recovery-{}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    std::env::set_var("RISK_STATE_PATH", "risk.json");
    journal::initialize().unwrap();
    let buy = OrderRequest {
        token_id: "123".into(),
        price: dec!(0.5),
        size: dec!(10),
        side: TradeSide::BUY,
        budget_usd: None,
    };
    assert!(journal::begin_contextual(&buy).is_err());
    journal::remember_source("123", "0xABC");
    let key = journal::begin_contextual(&buy).unwrap();
    let id = format!("0x{}", "a".repeat(64));
    journal::finish(&key, "accepted", Some(id.clone())).unwrap();
    let records = journal::snapshot().unwrap();
    assert_eq!(records[&key].source_wallet.as_deref(), Some("0xabc"));
    let now = chrono::Utc::now().timestamp();
    let fill = Fill {
        transaction_hash: format!("0x{}", "b".repeat(64)),
        block_hash: format!("0x{}", "c".repeat(64)),
        block_number: 1,
        block_timestamp: now,
        transaction_index: 0,
        log_index: 0,
        order_id: id.clone(),
        token_id: "123".into(),
        side: TradeSide::BUY,
        shares: dec!(4),
        collateral: dec!(2),
        fee: dec!(0.04),
    };
    live_fills::persist(std::slice::from_ref(&fill)).unwrap();
    assert_eq!(live_fills::stored().unwrap(), vec![fill.clone()]);
    let state = Arc::new(RwLock::new(BotState::default()));
    state.write().await.positions.insert(
        "123".into(),
        Position {
            token_id: "123".into(),
            size: dec!(100),
            average_entry_price: dec!(0.1),
        },
    );
    let ledger = Arc::new(Mutex::new(CopyLedger::load_from("ledger.json")));
    live_fills::apply(&live_fills::stored().unwrap(), &records, &state, &ledger)
        .await
        .unwrap();
    assert_eq!(state.read().await.reported_positions()["123"].size, dec!(4));
    assert_eq!(state.read().await.positions["123"].size, dec!(100));
    assert_eq!(
        state.read().await.copier_positions["123"].average_entry_price,
        dec!(0.51)
    );
    assert_eq!(ledger.lock().await.entries[0].filled_size, Some(dec!(4)));
    let restarted = Arc::new(Mutex::new(CopyLedger::load_from("ledger.json")));
    live_fills::apply(&live_fills::stored().unwrap(), &records, &state, &restarted)
        .await
        .unwrap();
    assert_eq!(restarted.lock().await.entries.len(), 1);

    let sell = OrderRequest {
        side: TradeSide::SELL,
        size: dec!(4),
        ..buy.clone()
    };
    journal::remember_source("123", "0xABC");
    let key = journal::begin_contextual(&sell).unwrap();
    let sale_id = format!("0x{}", "d".repeat(64));
    journal::finish(&key, "accepted", Some(sale_id.clone())).unwrap();
    let sale = Fill {
        order_id: sale_id,
        side: TradeSide::SELL,
        shares: dec!(4),
        collateral: dec!(1.6),
        fee: dec!(0.02),
        block_number: 2,
        transaction_hash: format!("0x{}", "e".repeat(64)),
        ..fill.clone()
    };
    live_fills::persist(&[fill.clone(), sale]).unwrap();
    live_fills::apply(
        &live_fills::stored().unwrap(),
        &journal::snapshot().unwrap(),
        &state,
        &restarted,
    )
    .await
    .unwrap();
    assert!(state.read().await.copier_positions.is_empty());
    assert_eq!(state.read().await.realized_pnl, dec!(-0.46));
    assert!(restarted.lock().await.entries[0].closed);
    journal::remember_source("123", "0xABC");
    let key = journal::begin_contextual(&buy).unwrap();
    let canceled_id = format!("0x{}", "f".repeat(64));
    journal::finish(&key, "accepted", Some(canceled_id.clone())).unwrap();
    live_fills::apply(
        &live_fills::stored().unwrap(),
        &journal::snapshot().unwrap(),
        &state,
        &restarted,
    )
    .await
    .unwrap();
    assert!(!restarted.lock().await.entries[1].closed);
    assert_eq!(restarted.lock().await.entries[1].filled_size, Some(dec!(0)));
    journal::cancel_unfilled(&canceled_id).unwrap();
    live_fills::apply(
        &live_fills::stored().unwrap(),
        &journal::snapshot().unwrap(),
        &state,
        &restarted,
    )
    .await
    .unwrap();
    assert!(restarted.lock().await.entries[1].closed);
    assert_eq!(state.read().await.realized_pnl, dec!(-0.46));
    let mut config = common::test_config();
    config.max_consecutive_losses = 3;
    config.loss_cooldown_secs = 900;
    let mut risk = RiskEngine::new(config.clone());
    live_fills::sync_risk(&mut risk).unwrap();
    let outcomes = vec![
        ("loss2".into(), dec!(-1), now),
        ("loss3".into(), dec!(-1), now),
    ];
    risk.sync_live_outcomes(&outcomes).unwrap();
    let raw = std::fs::read_to_string("risk.json").unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()["losses"],
        3
    );
    let mut restored = RiskEngine::new(config.clone());
    restored.restore_persistent().unwrap();
    live_fills::sync_risk(&mut restored).unwrap();
    restored.sync_live_outcomes(&outcomes).unwrap();
    assert_eq!(std::fs::read_to_string("risk.json").unwrap(), raw);
    let event = common::make_trade("0xabc", dec!(0.5), dec!(4), TradeSide::BUY);
    assert!(restored
        .check_execution(&event)
        .unwrap_err()
        .contains("cooldown"));
    std::env::set_var("RISK_STATE_PATH", "missing/risk.json");
    let mut fresh = RiskEngine::new(config.clone());
    assert!(fresh
        .check_execution(&event)
        .unwrap_err()
        .contains("persistence"));
    assert!(restored
        .sync_live_outcomes(&[("failed-win".into(), dec!(1), now)])
        .is_err());
    assert!(restored
        .check_execution(&event)
        .unwrap_err()
        .contains("cooldown"));
    std::env::set_var("RISK_STATE_PATH", "risk.json");
    restored
        .sync_live_outcomes(&[("failed-win".into(), dec!(1), now)])
        .unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("risk.json").unwrap()).unwrap();
    assert_eq!(raw["losses"], 0);
    assert!(raw["seen_live_outcomes"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("failed-win")));
    std::fs::write("settlement-fills.json", "corrupt").unwrap();
    assert!(live_fills::sync_risk(&mut restored).is_err());
    std::env::remove_var("RISK_STATE_PATH");
    std::env::set_current_dir(original).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
