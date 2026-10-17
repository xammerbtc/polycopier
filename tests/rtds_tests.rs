use polycopier::{
    config::{validate_config, BotConfig},
    rtds::{event_key, parse_activity},
};
#[test]
fn deduplicates_websocket_and_polling_price_precision() {
    let wallet = "0xabc".to_string();
    let mut frame = serde_json::json!({"topic":"activity","type":"trades","payload":{"proxyWallet":wallet,"transactionHash":"0xtx","asset":"123","price":"0.6999999719999955","size":"7.142856","side":"BUY","timestamp":12345}});
    let ws = parse_activity(&frame, std::slice::from_ref(&wallet)).unwrap();
    frame["payload"]["price"] = "0.699999972".into();
    frame["payload"]["timestamp"] = 12346.into();
    let poll = parse_activity(&frame, std::slice::from_ref(&wallet)).unwrap();
    assert_eq!(event_key(&ws), event_key(&poll));
    frame["payload"]["size"] = "8.142856".into();
    assert_ne!(
        event_key(&ws),
        event_key(&parse_activity(&frame, &[wallet]).unwrap())
    );
}
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
