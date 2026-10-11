use polycopier::{
    config::{BotConfig, Config},
    models::{Position, TradeEvent, TradeSide},
    risk::RiskEngine,
    simulation,
    state::BotState,
};
use rust_decimal_macros::dec;

#[test]
fn simulation_and_actual_daily_volume_survive_restart() {
    let original = std::env::current_dir().unwrap();
    let dir = std::env::temp_dir().join(format!("polycopier-runtime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let mut state = BotState::new(true, Some(dec!(200)));
    state.total_balance = dec!(192);
    state.positions.insert(
        "123".into(),
        Position {
            token_id: "123".into(),
            size: dec!(16),
            average_entry_price: dec!(0.5),
        },
    );
    simulation::save(&state).unwrap();
    let mut restored = BotState::new(true, Some(dec!(200)));
    simulation::restore(&mut restored).unwrap();
    assert_eq!(restored.total_balance, dec!(192));
    assert_eq!(restored.positions["123"].size, dec!(16));
    std::env::set_var("RISK_STATE_PATH", dir.join("runtime-risk.json"));
    let mut cfg = BotConfig::default();
    cfg.risk.max_daily_volume_usd = dec!(10);
    let cfg = Config::from_parts(String::new(), String::new(), cfg);
    let mut risk = RiskEngine::new(cfg.clone());
    let event = TradeEvent {
        transaction_hash: "tx".into(),
        maker_address: "a".into(),
        taker_address: "a".into(),
        token_id: "a".into(),
        price: dec!(0.5),
        size: dec!(16),
        side: TradeSide::BUY,
        timestamp: chrono::Utc::now().timestamp(),
    };
    let mut whale = event.clone();
    whale.size = dec!(1000);
    let mut stale = event.clone();
    stale.timestamp -= 60;
    assert!(risk
        .check_signal(&stale)
        .unwrap_err()
        .contains("Stale signal"));
    assert!(risk.check_signal(&whale).is_ok());
    assert!(risk.check_execution(&event).is_ok());
    let mut restored_risk = RiskEngine::new(cfg);
    restored_risk.restore_persistent().unwrap();
    let mut next = event;
    next.token_id = "b".into();
    assert!(restored_risk
        .check_execution(&next)
        .unwrap_err()
        .contains("Daily volume"));
    std::env::remove_var("RISK_STATE_PATH");
    std::env::set_current_dir(original).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
