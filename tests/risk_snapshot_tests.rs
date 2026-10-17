mod common;
use polycopier::{models::TradeSide, risk::RiskEngine};
use rust_decimal_macros::dec;

#[test]
fn late_live_outcomes_replay_in_chain_order_and_fail_closed() {
    let dir = std::env::temp_dir().join(format!("polycopier-risk-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("risk.json");
    let previous = std::env::var_os("RISK_STATE_PATH");
    std::env::set_var("RISK_STATE_PATH", &path);
    let mut config = common::test_config();
    config.max_consecutive_losses = 2;
    config.loss_cooldown_secs = 900;
    let now = chrono::Utc::now().timestamp();
    let win = ("win".into(), dec!(1), now);
    let loss = ("loss".into(), dec!(-1), now);
    let older_loss = ("older-loss".into(), dec!(-1), now);
    let mut risk = RiskEngine::new(config.clone());
    risk.sync_live_snapshot(&[win.clone(), loss.clone()])
        .unwrap();
    let timeline = vec![older_loss, win, loss];
    risk.sync_live_snapshot(&timeline).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(state["losses"], 1);
    assert!(state["cooldown_until"].is_null());
    // Equal timestamps retain the complete book's block / transaction / log order.
    assert_eq!(state["live_outcome_snapshot"][0][0], "older-loss");
    let mut restarted = RiskEngine::new(config.clone());
    restarted.restore_persistent().unwrap();
    restarted.sync_live_snapshot(&timeline).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    assert!(restarted.sync_live_snapshot(&timeline[1..]).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    let mut next = timeline.clone();
    next.push(("second-loss".into(), dec!(-1), now));
    std::env::set_var("RISK_STATE_PATH", dir.join("missing/risk.json"));
    assert!(restarted.sync_live_snapshot(&next).is_err());
    std::env::set_var("RISK_STATE_PATH", &path);
    restarted.sync_live_snapshot(&timeline).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    restarted.sync_live_snapshot(&next).unwrap();
    let event = common::make_trade("0xabc", dec!(0.5), dec!(4), TradeSide::BUY);
    assert!(restarted
        .check_execution(&event)
        .unwrap_err()
        .contains("cooldown"));
    let mut invalid = next.clone();
    invalid.push(("future".into(), dec!(1), now + 100));
    assert!(restarted.sync_live_snapshot(&invalid).is_err());
    // Expired historical cooldown must not add old losses to a new streak.
    let earlier = now - 901;
    if chrono::DateTime::from_timestamp(earlier, 0)
        .unwrap()
        .date_naive()
        == chrono::Utc::now().date_naive()
    {
        let mut expired = RiskEngine::new(config.clone());
        expired
            .sync_live_snapshot(&[
                ("old1".into(), dec!(-1), earlier),
                ("old2".into(), dec!(-1), earlier),
                ("new1".into(), dec!(-1), now),
            ])
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["losses"], 1);
        assert!(value["cooldown_until"].is_null());
    }
    // Emulate a crash after the paper balance/close is saved but before risk
    // receives its channel notification. Restart must recover exactly once.
    std::fs::remove_file(&path).unwrap();
    let mut prior = RiskEngine::new(config.clone());
    prior.record_loss();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let mut paper = polycopier::state::BotState::new(true, Some(dec!(194)));
    paper.positions.insert(
        "paper-token".into(),
        polycopier::models::Position {
            token_id: "paper-token".into(),
            size: dec!(10),
            average_entry_price: dec!(0.6),
        },
    );
    assert_eq!(
        polycopier::simulation::sell(&mut paper, "paper-token", dec!(10), dec!(0.5), dec!(0))
            .unwrap(),
        Some(dec!(-1))
    );
    std::fs::create_dir("simulation_state.json.tmp").unwrap();
    assert!(polycopier::simulation::commit(&mut paper).is_err());
    assert_eq!(paper.paper_committed_outcomes, 0);
    polycopier::simulation::sync_risk(&mut prior, &paper).unwrap();
    let not_committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(not_committed["losses"], 1);
    std::fs::remove_dir("simulation_state.json.tmp").unwrap();
    polycopier::simulation::commit(&mut paper).unwrap();
    let mut restored_paper = polycopier::state::BotState::new(true, None);
    polycopier::simulation::restore(&mut restored_paper).unwrap();
    assert_eq!(restored_paper.paper_outcomes, paper.paper_outcomes);
    let mut restored_risk = RiskEngine::new(config.clone());
    restored_risk.restore_persistent().unwrap();
    polycopier::simulation::sync_risk(&mut restored_risk, &restored_paper).unwrap();
    assert!(restored_risk
        .check_execution(&event)
        .unwrap_err()
        .contains("cooldown"));
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut replayed = RiskEngine::new(config);
    replayed.restore_persistent().unwrap();
    polycopier::simulation::sync_risk(&mut replayed, &restored_paper).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    assert!(replayed
        .check_execution(&event)
        .unwrap_err()
        .contains("cooldown"));
    std::env::set_current_dir(original).unwrap();
    if let Some(value) = previous {
        std::env::set_var("RISK_STATE_PATH", value);
    } else {
        std::env::remove_var("RISK_STATE_PATH");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
