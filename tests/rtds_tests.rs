use polycopier::{
    config::{validate_config, BotConfig},
    rtds::{event_key, parse_activity},
};
#[test]
fn filters_wallets_and_preserves_transaction_identity() {
    let wallet = "0x2d2d2b32e5199edf833594a99b9c54e5668ab69d".to_string();
    let mut frame = serde_json::json!({"topic":"activity","type":"trades","payload":{"proxyWallet":wallet.to_uppercase(),"transactionHash":"0xabc","asset":"123","price":0.5,"size":10,"side":"BUY","timestamp":12345}});
    let e = parse_activity(&frame, std::slice::from_ref(&wallet)).unwrap();
    assert_eq!(e.transaction_hash, "0xabc");
    assert!(parse_activity(&frame, &[]).is_none());
    frame["payload"]["asset"] = "456".into();
    assert_ne!(
        event_key(&e),
        event_key(&parse_activity(&frame, &[wallet]).unwrap())
    );
}
#[test]
fn rejects_invalid_wallets_and_risk_settings() {
    let mut cfg = BotConfig::default();
    cfg.targets.wallets = vec!["bad\"\n[risk]".into()];
    assert!(validate_config(&cfg).is_err());
    cfg.targets.wallets.clear();
    cfg.execution.max_trade_size_usd = (-1).into();
    assert!(validate_config(&cfg).is_err());
}
#[tokio::test]
async fn simulation_does_not_need_credentials_or_network() {
    let cfg =
        polycopier::config::Config::from_parts(String::new(), String::new(), BotConfig::default());
    assert!(polycopier::clients::build_sim_order_submitter(&cfg)
        .await
        .is_ok());
}
